use time::OffsetDateTime;

/// Supplies operational time without process-global clock access in domain code.
pub trait Clock: Send + Sync {
    fn now(&self) -> OffsetDateTime;
}

/// Production wall clock normalized to Coordinated Universal Time (UTC).
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> OffsetDateTime {
        OffsetDateTime::now_utc()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FixedClock(OffsetDateTime);

    impl Clock for FixedClock {
        fn now(&self) -> OffsetDateTime {
            self.0
        }
    }

    fn observe(clock: &dyn Clock) -> OffsetDateTime {
        clock.now()
    }

    #[test]
    fn time_sensitive_code_can_use_an_injected_clock() {
        let fixed = OffsetDateTime::from_unix_timestamp(1_777_734_400).unwrap();
        assert_eq!(observe(&FixedClock(fixed)), fixed);
        assert_eq!(SystemClock.now().offset(), time::UtcOffset::UTC);
    }
}
