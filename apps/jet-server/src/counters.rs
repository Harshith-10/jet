use std::sync::atomic::{AtomicU64, Ordering};

pub fn try_increment_with_limit(counter: &AtomicU64, max: u64) -> bool {
    let mut current = counter.load(Ordering::Relaxed);

    loop {
        if current >= max {
            return false;
        }

        match counter.compare_exchange_weak(
            current,
            current + 1,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return true,
            Err(observed) => current = observed,
        }
    }
}

pub fn saturating_decrement(counter: &AtomicU64) {
    let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        Some(current.saturating_sub(1))
    });
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::{saturating_decrement, try_increment_with_limit};

    #[test]
    fn refuses_increment_once_limit_is_reached() {
        let counter = AtomicU64::new(1);

        assert!(!try_increment_with_limit(&counter, 1));
        assert_eq!(counter.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn saturating_decrement_never_underflows() {
        let counter = AtomicU64::new(0);

        saturating_decrement(&counter);
        assert_eq!(counter.load(Ordering::Relaxed), 0);
    }
}
