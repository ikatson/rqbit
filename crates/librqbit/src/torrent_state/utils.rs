use std::sync::atomic::{AtomicU32, Ordering};

pub fn atomic_inc(c: &AtomicU32) -> u32 {
    c.fetch_add(1, Ordering::Relaxed)
}

pub fn atomic_dec(c: &AtomicU32) -> u32 {
    c.fetch_sub(1, Ordering::Relaxed)
}

// Used during debugging to see if some locks take too long.
#[cfg(not(feature = "timed_existence"))]
mod timed_existence {
    use std::ops::{Deref, DerefMut};

    pub struct TimedExistence<T>(T);

    impl<T> TimedExistence<T> {
        #[inline(always)]
        pub fn new(object: T, _reason: &'static str) -> Self {
            Self(object)
        }
    }

    impl<T> Deref for TimedExistence<T> {
        type Target = T;

        #[inline(always)]
        fn deref(&self) -> &Self::Target {
            &self.0
        }
    }

    impl<T> DerefMut for TimedExistence<T> {
        #[inline(always)]
        fn deref_mut(&mut self) -> &mut Self::Target {
            &mut self.0
        }
    }

    #[inline(always)]
    pub fn timeit<R>(_n: impl std::fmt::Display, f: impl FnOnce() -> R) -> R {
        f()
    }
}

#[cfg(feature = "timed_existence")]
mod timed_existence {
    use std::ops::{Deref, DerefMut};
    use std::time::{Duration, Instant};
    use tracing::warn;

    const MAX: Duration = Duration::from_millis(1);

    // Prints if the object exists for too long.
    // This is used to track long-lived locks for debugging.
    pub struct TimedExistence<T> {
        object: T,
        reason: &'static str,
        started: Instant,
    }

    impl<T> TimedExistence<T> {
        pub fn new(object: T, reason: &'static str) -> Self {
            Self {
                object,
                reason,
                started: Instant::now(),
            }
        }
    }

    impl<T> Drop for TimedExistence<T> {
        fn drop(&mut self) {
            let elapsed = self.started.elapsed();
            let reason = self.reason;
            if elapsed > MAX {
                warn!("elapsed on lock {reason:?}: {elapsed:?}")
            }
        }
    }

    impl<T> Deref for TimedExistence<T> {
        type Target = T;

        fn deref(&self) -> &Self::Target {
            &self.object
        }
    }

    impl<T> DerefMut for TimedExistence<T> {
        fn deref_mut(&mut self) -> &mut Self::Target {
            &mut self.object
        }
    }

    pub fn timeit<R>(name: impl std::fmt::Display, f: impl FnOnce() -> R) -> R {
        let now = Instant::now();
        let r = f();
        let elapsed = now.elapsed();
        if elapsed > MAX {
            warn!("elapsed on \"{name:}\": {elapsed:?}")
        }
        r
    }
}

pub use timed_existence::{timeit, TimedExistence};
