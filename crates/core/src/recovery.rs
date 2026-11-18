//! Retry only a provider request. Tool dispatch lives outside this boundary.
use super::{Cancel, Emit, Event};
use anyhow::Result;
use std::time::{Duration, Instant};

#[derive(Debug)]
pub struct TransientFailure {
    pub reason: String,
    pub retry_after: Option<Duration>,
}
impl std::fmt::Display for TransientFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.reason)
    }
}
impl std::error::Error for TransientFailure {}

pub(crate) fn request<T>(
    cancel: &Cancel,
    emit: &mut Emit,
    mut send: impl FnMut(&mut Emit) -> Result<T>,
) -> Result<T> {
    for attempt in 0.. {
        cancel.check()?;
        let mut streamed = false;
        let result = send(&mut |event| {
            streamed = true;
            emit(event);
        });
        let Err(error) = result else { return result };
        let Some(transient) = error.downcast_ref::<TransientFailure>() else {
            return Err(error);
        };
        if streamed || attempt >= 3 {
            return Err(error);
        }
        let backoff = Duration::from_secs(2 << attempt);
        let delay = transient.retry_after.unwrap_or(backoff);
        if delay > Duration::from_secs(60) {
            return Err(error.context(format!("retry requested in {}s", delay.as_secs())));
        }
        emit(Event::Retry {
            attempt: attempt + 1,
            seconds: delay.as_secs_f64().ceil() as u64,
        });
        let deadline = Instant::now() + delay;
        loop {
            cancel.check()?;
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            std::thread::sleep(remaining.min(Duration::from_millis(40)));
        }
    }
    unreachable!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn retry_sequences(failures in 0..7usize, streamed in any::<bool>(), permanent in any::<bool>()) {
            let mut requests = 0;
            let mut retries = 0;
            let result = request(&Cancel::default(), &mut |event| {
                if matches!(event,Event::Retry{..}) {retries+=1}
            }, |emit| {
                requests+=1;
                if requests > failures {return Ok(())}
                if streamed {emit(Event::Text("partial".into()))}
                if permanent {anyhow::bail!("permanent failure")}
                Err(TransientFailure{reason:"temporary".into(),retry_after:Some(Duration::ZERO)}.into())
            });
            let allowed = if streamed || permanent {0} else {3};
            prop_assert_eq!(retries, failures.min(allowed));
            prop_assert_eq!(requests,retries+1);
            prop_assert_eq!(result.is_ok(),failures<=allowed);
        }
    }
    #[test]
    fn cancelling_countdown_never_sends_again() {
        let cancel = Cancel::default();
        let mut calls = 0;
        let err = request::<()>(&cancel, &mut |_| cancel.stop(), |_| {
            calls += 1;
            Err(TransientFailure {
                reason: "unavailable".into(),
                retry_after: None,
            }
            .into())
        })
        .unwrap_err();
        assert_eq!(err.to_string(), "cancelled");
        assert_eq!(calls, 1);
    }
}
