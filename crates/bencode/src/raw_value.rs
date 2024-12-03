use serde::Serialize;

pub struct RawValue<T>(pub T);

pub(crate) const TAG: &str = "::librqbit_bencode::RawValue";

impl<T> Serialize for RawValue<T>
where
    T: AsRef<[u8]>,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        struct Wrapper<'a>(&'a [u8]);

        impl Serialize for Wrapper<'_> {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                serializer.serialize_bytes(self.0)
            }
        }

        serializer.serialize_newtype_struct(TAG, &Wrapper(self.0.as_ref()))
    }
}
