// Run a function with initial interval, then run it forever with the interval that the
// callback returns.
// Returns a callback to clear it.

export function customSetInterval(
  asyncCallback: () => Promise<number>,
  initialInterval: number
): () => void {
  let timeoutId: any;
  let currentInterval: number = initialInterval;

  const executeCallback = async () => {
    currentInterval = await asyncCallback();
    if (currentInterval === null || currentInterval === undefined) {
      throw "asyncCallback returned null or undefined";
    }
    scheduleNext();
  };

  let scheduleNext = () => {
    timeoutId = setTimeout(executeCallback, currentInterval);
  };

  scheduleNext();

  return () => {
    clearTimeout(timeoutId);
  };
}
