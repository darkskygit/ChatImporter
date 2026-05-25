use super::discovery::{discover_backup_candidates, BackupCandidate};
use anyhow::Result;
use std::path::PathBuf;

#[derive(Debug, Default)]
pub struct ImportSummary {
    pub imported: usize,
    pub skipped: usize,
    pub failed: usize,
}

#[derive(Debug, Default)]
pub struct ImportSession {
    candidates: Vec<BackupCandidate>,
    summary: ImportSummary,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
// Kept for the batch session runner used by tests and future unified backup import flows.
#[allow(dead_code)]
pub enum CandidateDisposition {
    Imported,
    Skipped,
}

impl ImportSession {
    pub fn new(candidates: Vec<BackupCandidate>) -> Self {
        Self {
            candidates,
            summary: ImportSummary::default(),
        }
    }

    pub fn discover(paths: &[PathBuf]) -> Result<Self> {
        discover_backup_candidates(paths).map(Self::new)
    }

    pub fn candidates(&self) -> &[BackupCandidate] {
        &self.candidates
    }

    pub fn summary(&self) -> &ImportSummary {
        &self.summary
    }

    pub fn mark_imported(&mut self) {
        self.summary.imported += 1;
    }

    pub fn mark_skipped(&mut self) {
        self.summary.skipped += 1;
    }

    pub fn mark_failed(&mut self) {
        self.summary.failed += 1;
    }

    // Kept for a future shared runner once WeChat/SMS/iMazing import loops are consolidated.
    #[allow(dead_code)]
    pub fn run<F>(&mut self, mut import: F)
    where
        F: FnMut(&BackupCandidate) -> Result<CandidateDisposition>,
    {
        for candidate in self.candidates.clone() {
            match import(&candidate) {
                Ok(CandidateDisposition::Imported) => self.mark_imported(),
                Ok(CandidateDisposition::Skipped) => self.mark_skipped(),
                Err(error) => {
                    log::error!(
                        "failed to import backup candidate {} ({}): {}",
                        candidate.source_id,
                        candidate.display_path.display(),
                        error
                    );
                    self.mark_failed();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backup::discovery::BackupCandidateKind;

    fn candidate(id: &str) -> BackupCandidate {
        BackupCandidate {
            source_id: id.into(),
            display_path: PathBuf::from(id),
            kind: BackupCandidateKind::StandardRoot(PathBuf::from(id)),
        }
    }

    #[test]
    fn candidate_failure_does_not_interrupt_session() {
        let mut session = ImportSession::new(vec![
            candidate("first"),
            candidate("second"),
            candidate("third"),
        ]);

        session.run(|candidate| {
            if candidate.source_id == "second" {
                anyhow::bail!("boom");
            }
            Ok(CandidateDisposition::Imported)
        });

        assert_eq!(session.summary().imported, 2);
        assert_eq!(session.summary().failed, 1);
        assert_eq!(session.summary().skipped, 0);
    }
}
