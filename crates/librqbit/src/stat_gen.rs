macro_rules! stype {
    (atomic u32) => {
        std::sync::atomic::AtomicU32
    };
    (atomic u64) => {
        std::sync::atomic::AtomicU64
    };
    (u32) => {
        u32
    };
    (u64) => {
        u64
    };
}

macro_rules! gen_stats {
    ($atomic_name:ident $snapshot_name:ident, [$($stat_name:ident $stat_ty:tt),*], [$($nested_field_name:ident $nested_atomic_name:ident $nested_snapshot_name:ident),*]) => {
        #[derive(Debug, Default)]
        pub struct $atomic_name {
            $(
                pub $stat_name: stype!(atomic $stat_ty),
            )*

            $(
                pub $nested_field_name: $nested_atomic_name,
            )*
        }

        impl $atomic_name {
            pub fn snapshot(&self) -> $snapshot_name {
                $snapshot_name {
                    $(
                        $stat_name: self.$stat_name.load(std::sync::atomic::Ordering::Relaxed),
                    )*

                    $(
                        $nested_field_name: self.$nested_field_name.snapshot(),
                    )*
                }
            }

            $(
                pub fn $stat_name(&self, value: $stat_ty) {
                    self.$stat_name.fetch_add(value, std::sync::atomic::Ordering::Relaxed);
                }
            )*
        }

        #[derive(Debug, serde::Serialize)]
        pub struct $snapshot_name {
            $(
                pub $stat_name: stype!($stat_ty),
            )*

            $(
                pub $nested_field_name: $nested_snapshot_name,
            )*
        }
    };
}
