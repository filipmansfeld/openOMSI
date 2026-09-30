//! Cancellation and receipts shared by the socket worker and frame thread.
use serde_json::{json, Value};
use std::sync::{atomic::{AtomicU8, Ordering}, mpsc, Arc};
use std::time::{Duration, Instant};

const QUEUED: u8 = 0;
const STARTED: u8 = 1;
const CANCELLED: u8 = 2;

#[derive(Default)]
pub(super) struct State(AtomicU8);
impl State {
    /// Cancellation and starting compete for the same state. A queued timeout can
    /// therefore never be reported as rejected and subsequently start executing.
    pub(super) fn begin(&self, deadline: Instant) -> bool {
        if Instant::now() > deadline { self.cancel(); return false; }
        self.0.compare_exchange(QUEUED,STARTED,Ordering::AcqRel,Ordering::Acquire).is_ok()
    }

    fn cancel(&self) -> bool {
        match self.0.compare_exchange(QUEUED,CANCELLED,Ordering::AcqRel,Ordering::Acquire) {
            Ok(_) => true,
            Err(value) => value == CANCELLED,
        }
    }
}

pub(super) struct Receipt {
    pub(super) state: Arc<State>,
    receiver: mpsc::Receiver<Result<Value,String>>,
    completed: Option<Value>,
}
impl Receipt {
    pub(super) fn channel() -> (mpsc::SyncSender<Result<Value,String>>,Self) {
        let (sender,receiver) = mpsc::sync_channel(1);
        (sender,Self { state:Arc::new(State::default()),receiver,completed:None })
    }

    pub(super) fn reject(&mut self, id: u64, error: &str) -> Value {
        self.state.cancel();
        let response = json!({"protocol":1,"request_id":id,"ok":false,"status":"rejected","error":error});
        self.completed = Some(response.clone());
        response
    }

    pub(super) fn response(&mut self, id: u64, timeout: Duration) -> Value {
        if let Some(response) = &self.completed { return response.clone(); }
        match self.receiver.recv_timeout(timeout) {
            Ok(result) => {
                let response = match result {
                    Ok(value) => json!({"protocol":1,"request_id":id,"ok":true,"result":value}),
                    Err(error) => json!({"protocol":1,"request_id":id,"ok":false,"error":error}),
                };
                // Cache just one bounded response per connection. A large read must
                // use pagination; a large write result cannot imply rollback.
                let response = if serde_json::to_vec(&response).map_or(true,|v|v.len()>super::protocol::MAX_JSON) {
                    json!({"protocol":1,"request_id":id,"ok":false,"status":"completed_reply_too_large",
                        "error":"operation completed but its reply exceeded the size limit; state may have changed; use a smaller read to inspect it"})
                } else { response };
                self.completed = Some(response.clone());
                response
            }
            Err(_) if self.state.cancel() => self.reject(id,"command expired or disconnected before execution; no changes were applied"),
            Err(_) => json!({"protocol":1,"request_id":id,"ok":false,"status":"indeterminate",
                "error":"command already started but its acknowledgement is not ready; it may change state; retry this same request_id on this connection to retrieve its receipt without reapplying"}),
        }
    }
}

/// Only the latest command's receipt is retained. IDs below it stay rejected, so
/// discarding old receipts never allows an old mutation to execute a second time.
#[derive(Default)]
pub(super) struct ReplayGuard { highest: Option<u64> }
impl ReplayGuard {
    pub(super) fn accept_new(&mut self, id: u64) -> bool {
        if self.highest.is_some_and(|previous| id <= previous) { return false; }
        self.highest = Some(id);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queued_timeout_cannot_start_later() {
        let (_sender,mut receipt) = Receipt::channel();
        assert_eq!(receipt.response(1,Duration::ZERO)["status"],"rejected");
        assert!(!receipt.state.begin(Instant::now()+Duration::from_secs(1)));
    }

    #[test]
    fn running_timeout_keeps_receipt_and_never_starts_twice() {
        let (sender,mut receipt) = Receipt::channel();
        assert!(receipt.state.begin(Instant::now()+Duration::from_secs(1)));
        assert_eq!(receipt.response(2,Duration::ZERO)["status"],"indeterminate");
        sender.send(Ok(json!({"applied":1}))).unwrap();
        let result = receipt.response(2,Duration::ZERO);
        assert_eq!(result["result"]["applied"],1);
        assert_eq!(receipt.response(2,Duration::ZERO),result);
        assert!(!receipt.state.begin(Instant::now()+Duration::from_secs(1)));
    }

    #[test]
    fn expired_and_replayed_commands_are_never_accepted() {
        assert!(!State::default().begin(Instant::now()-Duration::from_secs(1)));
        let mut ids = ReplayGuard::default();
        assert!(ids.accept_new(0));
        assert!(!ids.accept_new(0));
        assert!(ids.accept_new(7));
        assert!(!ids.accept_new(3));
        assert!(!ids.accept_new(7));
        assert!(ids.accept_new(u64::MAX));
        assert!(!ids.accept_new(0));
    }
}
