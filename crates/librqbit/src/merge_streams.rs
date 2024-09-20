use futures::stream::Stream;

pub fn merge_streams<
    I,
    S1: Stream<Item = I> + 'static + Unpin,
    S2: Stream<Item = I> + 'static + Unpin,
>(
    s1: S1,
    s2: S2,
) -> impl Stream<Item = I> + Unpin + 'static {
    use tokio_stream::StreamExt;
    s1.merge(s2)
}
