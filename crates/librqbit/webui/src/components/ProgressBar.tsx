type Props = {
  now: number;
  label?: string | null;
  variant?: "warn" | "info" | "success" | "error";
};

export const ProgressBar = ({ now, variant, label }: Props) => {
  const progressLabel = label ?? `${now.toFixed(2)}%`;

  const variantClassName = {
    warn: "bg-yellow-500",
    info: "bg-blue-500 text-white",
    success: "bg-green-700 text-white",
    error: "bg-red-500 text-white",
  }[variant ?? "info"];

  return (
    <div className={"w-full bg-gray-200 rounded-full dark:bg-gray-500"}>
      <div
        className={`text-xs bg-blue-500 font-medium transition-all text-center p-0.5 leading-none rounded-full ${variantClassName}`}
        style={{ width: `${now}%` }}
      >
        {progressLabel}
      </div>
    </div>
  );
};
