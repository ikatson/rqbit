macro_rules! stype {
    (atomic u32) => {
        portable_atomic::AtomicU32
    };
    (atomic u64) => {
        portable_atomic::AtomicU64
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
                        $stat_name: self.$stat_name.load(portable_atomic::Ordering::Relaxed),
                    )*

                    $(
                        $nested_field_name: self.$nested_field_name.snapshot(),
                    )*
                }
            }

            $(
                #[allow(unused)]
                pub fn $stat_name(&self, value: $stat_ty) {
                    self.$stat_name.fetch_add(value, portable_atomic::Ordering::Relaxed);
                }
            )*
        }

        #[derive(Debug, Default, serde::Serialize)]
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
