//! The one way Margin takes a `std::sync::Mutex`.

use std::sync::{Mutex, MutexGuard, PoisonError};

/// `Mutex::lock` under the poison policy: a poisoned lock is a bug (a panic while it
/// was held, which the panic's own report names), and the next caller takes the state
/// as the panic left it. That holds only because every critical section behind
/// `locked` leaves its state valid at each point it could panic (a slot set, a map
/// entry added or removed, a cache entry evicted); a lock whose state a panic could
/// leave half-written (an ONNX session that panicked mid-run) must not use this and
/// fails its request with a typed error instead. Never `unwrap()` a lock: one panic
/// would then fail every later request.
pub trait Locked<T> {
    fn locked(&self) -> MutexGuard<'_, T>;
}

impl<T> Locked<T> for Mutex<T> {
    fn locked(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_poisoned_lock_hands_over_the_state_the_panic_left() {
        let m = Mutex::new(vec![1]);
        let _ = std::panic::catch_unwind(|| {
            let mut v = m.lock().unwrap();
            v.push(2);
            panic!("while holding the lock");
        });
        assert!(m.is_poisoned());
        assert_eq!(*m.locked(), [1, 2]);
    }
}
