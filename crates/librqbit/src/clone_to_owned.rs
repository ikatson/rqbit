pub trait CloneToOwned {
    type Target;

    fn clone_to_owned(&self) -> Self::Target;
}

impl<T> CloneToOwned for Option<T>
where
    T: CloneToOwned,
{
    type Target = Option<<T as CloneToOwned>::Target>;

    fn clone_to_owned(&self) -> Self::Target {
        self.as_ref().map(|i| i.clone_to_owned())
    }
}

impl<T> CloneToOwned for Vec<T>
where
    T: CloneToOwned,
{
    type Target = Vec<<T as CloneToOwned>::Target>;

    fn clone_to_owned(&self) -> Self::Target {
        self.iter().map(|i| i.clone_to_owned()).collect()
    }
}
