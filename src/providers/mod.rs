pub mod browser_history;
pub mod windows;

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, mpsc, oneshot};

use crate::config::Config;
use crate::runtime::{Cursor, RecordDraft};

pub const RESTART_BACKOFF_START: Duration = Duration::from_secs(1);
pub const RESTART_BACKOFF_CEILING: Duration = Duration::from_secs(300);

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ProviderError(pub String);

#[derive(Clone)]
pub struct Ctx {
    pub config: Arc<Config>,
}

#[derive(Debug)]
pub struct Emission {
    pub records: Vec<RecordDraft>,
    pub cursor: Option<Cursor>,
    pub committed: Option<oneshot::Sender<()>>,
}

impl Emission {
    pub fn new(records: Vec<RecordDraft>) -> Emission {
        Emission {
            records,
            cursor: None,
            committed: None,
        }
    }

    pub fn with_cursor(records: Vec<RecordDraft>, cursor: Cursor) -> Emission {
        Emission {
            records,
            cursor: Some(cursor),
            committed: None,
        }
    }

    pub fn awaiting_commit(records: Vec<RecordDraft>) -> (Emission, oneshot::Receiver<()>) {
        let (committed, receipt) = oneshot::channel();
        let emission = Emission {
            records,
            cursor: None,
            committed: Some(committed),
        };
        (emission, receipt)
    }

    pub fn commit(self) {
        let Emission { committed, .. } = self;
        let Some(committed) = committed else {
            return;
        };
        let _ = committed.send(());
    }
}

pub trait Provider {
    fn name(&self) -> &'static str;
    fn run(
        &mut self,
        ctx: Ctx,
        out: mpsc::Sender<Emission>,
    ) -> impl Future<Output = Result<(), ProviderError>> + Send;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    pub start: Duration,
    pub ceiling: Duration,
}

impl Default for Backoff {
    fn default() -> Backoff {
        Backoff {
            start: RESTART_BACKOFF_START,
            ceiling: RESTART_BACKOFF_CEILING,
        }
    }
}

pub async fn supervise<P: Provider + Send + 'static>(
    provider: P,
    ctx: Ctx,
    out: mpsc::Sender<Emission>,
    backoff: Backoff,
) {
    let name = provider.name();
    let provider = Arc::new(Mutex::new(provider));
    let Backoff { start, ceiling } = backoff;
    let mut pause = start;

    loop {
        let attempt = {
            let provider = Arc::clone(&provider);
            let ctx = ctx.clone();
            let out = out.clone();
            tokio::spawn(async move {
                let mut provider = provider.lock().await;
                provider.run(ctx, out).await
            })
        };

        match attempt.await {
            Ok(Ok(())) => {
                tracing::info!(
                    provider = name,
                    "the provider finished and will not restart"
                );
                return;
            }
            Ok(Err(error)) => {
                tracing::error!(provider = name, %error, "the provider failed");
            }
            Err(join) => {
                if join.is_cancelled() {
                    return;
                }
                tracing::error!(provider = name, "the provider panicked");
            }
        }

        tracing::warn!(
            provider = name,
            pause_ms = pause.as_millis(),
            "the provider restarts after a pause"
        );
        tokio::time::sleep(pause).await;
        pause = (pause * 2).min(ceiling);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use url::Url;

    use crate::config::{Browser, Buffer, Keep, RedactRule};
    use crate::runtime::redact::WILDCARD_HOST;

    pub fn test_config(tick_interval: u64) -> Config {
        Config {
            service_url: Url::parse("http://alpha:8080").expect("the test url parses"),
            device: "mbp-21".to_string(),
            tick_interval,
            history_poll_interval: 300,
            revisit_window: 500,
            browser: Browser {
                profile: "MBP_21".to_string(),
            },
            buffer: Buffer {
                max_rows: 200_000,
                max_bytes: 536_870_912,
            },
            redact: vec![RedactRule {
                url_host: Some(WILDCARD_HOST.to_string()),
                keep: Some(Keep::Host),
                bundle_id: None,
                drop: Vec::new(),
            }],
            state_dir: std::env::temp_dir().join("nikki-provider-tests"),
        }
    }

    pub fn test_ctx(tick_interval: u64) -> Ctx {
        Ctx {
            config: Arc::new(test_config(tick_interval)),
        }
    }

    struct Flaky {
        attempts: Arc<AtomicUsize>,
        failures: usize,
    }

    impl Provider for Flaky {
        fn name(&self) -> &'static str {
            "flaky"
        }

        async fn run(
            &mut self,
            _ctx: Ctx,
            _out: mpsc::Sender<Emission>,
        ) -> Result<(), ProviderError> {
            let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
            if attempt < self.failures {
                return Err(ProviderError(format!("attempt {attempt} failed")));
            }
            Ok(())
        }
    }

    struct Exploding {
        attempts: Arc<AtomicUsize>,
    }

    impl Provider for Exploding {
        fn name(&self) -> &'static str {
            "exploding"
        }

        async fn run(
            &mut self,
            _ctx: Ctx,
            _out: mpsc::Sender<Emission>,
        ) -> Result<(), ProviderError> {
            let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
            if attempt == 0 {
                panic!("the provider fell over");
            }
            Ok(())
        }
    }

    fn quick_backoff() -> Backoff {
        Backoff {
            start: Duration::from_millis(1),
            ceiling: Duration::from_millis(4),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_failing_provider_is_restarted_until_it_finishes() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let provider = Flaky {
            attempts: Arc::clone(&attempts),
            failures: 3,
        };
        let (out, _inbox) = mpsc::channel(4);

        supervise(provider, test_ctx(30), out, quick_backoff()).await;

        assert_eq!(attempts.load(Ordering::SeqCst), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn a_panicking_provider_restarts_rather_than_taking_the_process_down() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let provider = Exploding {
            attempts: Arc::clone(&attempts),
        };
        let (out, _inbox) = mpsc::channel(4);

        supervise(provider, test_ctx(30), out, quick_backoff()).await;

        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_provider_that_finishes_cleanly_is_not_restarted() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let provider = Flaky {
            attempts: Arc::clone(&attempts),
            failures: 0,
        };
        let (out, _inbox) = mpsc::channel(4);

        supervise(provider, test_ctx(30), out, quick_backoff()).await;

        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn an_emission_awaiting_a_commit_is_released_by_it() {
        let (emission, receipt) = Emission::awaiting_commit(Vec::new());
        emission.commit();
        assert!(receipt.blocking_recv().is_ok());
    }

    #[test]
    fn an_ordinary_emission_carries_no_commit_handle() {
        let Emission {
            records,
            cursor,
            committed,
        } = Emission::new(Vec::new());
        assert!(records.is_empty());
        assert!(cursor.is_none());
        assert!(committed.is_none());
    }
}
