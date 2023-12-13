export function loopUntilSuccess<T>(
  callback: () => Promise<T>,
  interval: number
): () => void {
  let timeoutId: any;

  const executeCallback = async () => {
    let retry = await callback().then(
      () => false,
      () => true
    );
    if (retry) {
      scheduleNext();
    }
  };

  let scheduleNext = (overrideInterval?: number) => {
    timeoutId = setTimeout(
      executeCallback,
      overrideInterval !== undefined ? overrideInterval : interval
    );
  };

  scheduleNext(0);

  return () => clearTimeout(timeoutId);
}
