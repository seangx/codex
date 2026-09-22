//! Keep hidden reasoning and terminal bookkeeping inside compatible activity groups.
//! Detailed output retains the original order while compact activity avoids artificial breaks.

use super::*;

impl ChatWidget {
    pub(super) fn absorb_activity_detail(
        &mut self,
        cell: Box<dyn HistoryCell>,
    ) -> Result<(), Box<dyn HistoryCell>> {
        let hidden_reasoning = cell
            .as_any()
            .downcast_ref::<history_cell::ReasoningSummaryCell>()
            .is_some_and(history_cell::ReasoningSummaryCell::is_transcript_only);
        if !hidden_reasoning
            && !cell
                .as_any()
                .is::<history_cell::UnifiedExecInteractionCell>()
        {
            return Err(cell);
        }
        let Some(active) = self.transcript.active_cell.as_mut() else {
            return Err(cell);
        };
        if let Some(computer) = active
            .as_any_mut()
            .downcast_mut::<history_cell::ComputerActivityCell>()
        {
            computer.group.push_detail(Arc::from(cell));
            self.bump_active_cell_revision();
            return Ok(());
        }
        let exploration = if active.as_any().is::<ExecCell>() {
            active.as_any_mut().downcast_mut::<ExecCell>()
        } else {
            active
                .as_any_mut()
                .downcast_mut::<ToolActivityGroupCell>()
                .and_then(ToolActivityGroupCell::last_exec_mut)
        };
        if let Some(exploration) = exploration
            && exploration.is_exploring_cell()
        {
            exploration.group.push_detail(Arc::from(cell));
            self.bump_active_cell_revision();
            return Ok(());
        }
        Err(cell)
    }

    /// Hydrate only the validated older portion, leaving current calls and their clocks intact.
    pub(crate) fn prepend_active_exploration_history(
        &mut self,
        older: &dyn HistoryCell,
        turns: &[Turn],
    ) -> Option<(u64, u64)> {
        let previous_revision = self.transcript.active_cell_revision;
        let active_cell = self.transcript.active_cell.as_mut()?;
        let active = if active_cell.as_any().is::<ExecCell>() {
            active_cell.as_any_mut().downcast_mut::<ExecCell>()?
        } else {
            active_cell
                .as_any_mut()
                .downcast_mut::<ToolActivityGroupCell>()?
                .last_exec_mut()?
        };
        let older = crate::thread_transcript::older_exploration_group(older, active, turns)?;
        active.prepend(older);
        self.bump_active_cell_revision();
        Some((previous_revision, self.transcript.active_cell_revision))
    }
}
