use super::*;
use codex_rmcp_client::InProcessTransportFactory;
use futures::FutureExt;
use futures::future::BoxFuture;
use pretty_assertions::assert_eq;
use rmcp::ServiceExt;
use rmcp::model::CustomNotification;
use rmcp::model::Implementation;
use rmcp::model::InitializeRequestParams;
use tokio::sync::mpsc;
use tokio::time::timeout;

#[derive(Clone)]
struct NotificationServer(mpsc::Sender<serde_json::Value>);

impl rmcp::ServerHandler for NotificationServer {
    async fn initialize(
        &self,
        request: InitializeRequestParams,
        context: rmcp::service::RequestContext<rmcp::service::RoleServer>,
    ) -> std::result::Result<rmcp::model::InitializeResult, rmcp::ErrorData> {
        // Model the installed Computer History decoder's actual failure mode.
        if request.client_info.name == "computer-history"
            && request
                .capabilities
                .experimental
                .as_ref()
                .is_some_and(|caps| !caps.is_empty())
        {
            return Err(rmcp::ErrorData::internal_error(
                "incompatible experimental capabilities",
                /*data*/ None,
            ));
        }
        self.0.send(json!(request.capabilities)).await.unwrap();
        context.peer.set_peer_info(request);
        Ok(self.get_info())
    }

    async fn on_custom_notification(
        &self,
        notification: CustomNotification,
        _context: rmcp::service::NotificationContext<rmcp::RoleServer>,
    ) {
        self.0.send(json!(notification)).await.unwrap();
    }
}

impl InProcessTransportFactory for NotificationServer {
    fn open(&self) -> BoxFuture<'static, std::io::Result<tokio::io::DuplexStream>> {
        let server = self.clone();
        async move {
            let (client, transport) = tokio::io::duplex(/*max_buf_size*/ 4096);
            tokio::spawn(async move {
                let service = server.serve(transport).await.unwrap();
                service.waiting().await.unwrap();
            });
            Ok(client)
        }
        .boxed()
    }
}

#[tokio::test]
async fn auth_notifications_require_opt_in_and_follow_client_lifetime() -> Result<()> {
    let (notifications, mut received) = mpsc::channel(/*buffer*/ 8);
    let client = Arc::new(
        RmcpClient::new_in_process_client(Arc::new(NotificationServer(notifications))).await?,
    );
    client
        .initialize(
            InitializeRequestParams::new(
                ClientCapabilities::default(),
                Implementation::new("test", "1"),
            ),
            Some(SEND_TIMEOUT),
            Box::new(|_, _| async { anyhow::bail!("unexpected elicitation") }.boxed()),
        )
        .await?;
    let (changes, receiver) = watch::channel(AuthChangeState::default());
    assert_eq!(received.recv().await, Some(json!({})));
    let mut capabilities = ServerCapabilities::default();
    assert!(
        start(Arc::clone(&client), &capabilities, Some(receiver.clone()))
            .await?
            .is_none()
    );
    capabilities.experimental = Some([(CAPABILITY.to_string(), Default::default())].into());
    assert!(
        start(Arc::clone(&client), &capabilities, /*changes*/ None)
            .await?
            .is_none()
    );
    assert_eq!(received.try_recv(), Err(mpsc::error::TryRecvError::Empty));
    let watcher = start(Arc::clone(&client), &capabilities, Some(receiver))
        .await?
        .unwrap();
    assert_eq!(
        timeout(SEND_TIMEOUT, received.recv()).await?,
        Some(
            json!({"method": NOTIFICATION, "params": {"_meta": {}, "generation": 0, "ownerGeneration": 0}})
        ),
    );
    changes.send_modify(|state| state.generation += 1);
    assert_eq!(
        timeout(SEND_TIMEOUT, received.recv()).await?,
        Some(
            json!({"method": NOTIFICATION, "params": {"_meta": {}, "generation": 1, "ownerGeneration": 0}})
        ),
    );
    for _ in 0..2 {
        changes.send_modify(|state| {
            state.generation += 1;
            state.owner_generation += 1;
        });
    }
    assert_eq!(
        timeout(SEND_TIMEOUT, received.recv()).await?,
        Some(
            json!({"method": NOTIFICATION, "params": {"_meta": {}, "generation": 3, "ownerGeneration": 2}})
        ),
    );
    drop(watcher);
    timeout(SEND_TIMEOUT, changes.closed()).await?;
    client.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn history_handshake_omits_auth_advertisement_without_changing_other_servers() -> Result<()> {
    for name in ["computer-history", "other-server"] {
        let (notifications, mut received) = mpsc::channel(/*buffer*/ 8);
        let client =
            RmcpClient::new_in_process_client(Arc::new(NotificationServer(notifications))).await?;
        let mut capabilities = ClientCapabilities::default();
        // Unrelated extension capabilities must survive the compatibility policy.
        capabilities.extensions = Some([("openai/form".to_string(), Default::default())].into());
        advertise(name, &mut capabilities);
        client
            .initialize(
                InitializeRequestParams::new(capabilities, Implementation::new(name, "1")),
                Some(SEND_TIMEOUT),
                Box::new(|_, _| async { anyhow::bail!("unexpected elicitation") }.boxed()),
            )
            .await?;
        let expected = if name == "computer-history" {
            json!({"extensions": {"openai/form": {}}})
        } else {
            json!({"extensions": {"openai/form": {}}, "experimental": {"codex/auth-change": {}}})
        };
        assert_eq!(
            timeout(SEND_TIMEOUT, received.recv()).await?,
            Some(expected)
        );
        client.shutdown().await;
    }
    Ok(())
}
