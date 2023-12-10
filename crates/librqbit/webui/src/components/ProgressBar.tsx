type Props = {
  now: number;
  error?: string | null;
  finished: boolean;
  initializaion: boolean;
};

export const ProgressBar = ({ now, error, finished, initializaion }: Props) => {
  const progressLabel = error ? "Error" : `${now.toFixed(2)}%`;

  return (
    <div className={"w-full bg-gray-200 rounded-full"}>
      <div
        className="text-xs bg-blue-500 font-medium text-blue-100 text-center p-0.5 leading-none rounded-full"
        style={{ width: `${now}%` }}
      >
        {progressLabel}
      </div>
    </div>
  );
};
