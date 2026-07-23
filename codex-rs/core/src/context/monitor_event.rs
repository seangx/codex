use serde::Serialize;

use super::ContextualUserFragment;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MonitorEventStatus {
    Running,
    Exited { exit_code: Option<i32> },
    DeliveryLimitReached,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct MonitorEvent {
    monitor_id: i32,
    status: MonitorEventStatus,
    lines: Vec<String>,
    omitted_lines: usize,
}

impl MonitorEvent {
    pub(crate) fn running(monitor_id: i32, lines: Vec<String>, omitted_lines: usize) -> Self {
        Self {
            monitor_id,
            status: MonitorEventStatus::Running,
            lines,
            omitted_lines,
        }
    }

    pub(crate) fn exited(
        monitor_id: i32,
        exit_code: Option<i32>,
        lines: Vec<String>,
        omitted_lines: usize,
    ) -> Self {
        Self {
            monitor_id,
            status: MonitorEventStatus::Exited { exit_code },
            lines,
            omitted_lines,
        }
    }

    pub(crate) fn delivery_limit_reached(monitor_id: i32) -> Self {
        Self {
            monitor_id,
            status: MonitorEventStatus::DeliveryLimitReached,
            lines: Vec::new(),
            omitted_lines: 0,
        }
    }
}

impl ContextualUserFragment for MonitorEvent {
    fn role(&self) -> &'static str {
        "user"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("<monitor_event>", "</monitor_event>")
    }

    fn body(&self) -> String {
        format!(
            "\n{}\n",
            serde_json::to_string(self).unwrap_or_else(|_| {
                r#"{"status":"failed_to_serialize_monitor_event"}"#.to_string()
            })
        )
    }
}
