type Props = {
  now: number;
  label?: string | null;
  variant?: "warn" | "info" | "success" | "error";
};

const variantClassNames = {
  warn: "bg-amber-500 text-white",
  info: "bg-blue-500 text-white",
  success: "bg-green-700 text-white",
  error: "bg-red-500 text-white",
};

export const ProgressBar = ({ now, variant, label }: Props) => {
  const progressLabel = label ?? `${now.toFixed(2)}%`;

  const variantClassName =
    variantClassNames[variant ?? "info"] ?? variantClassNames["info"];

  return (
    <div className={"w-full bg-gray-200 rounded-full dark:bg-gray-500"}>
      <div
        className={`text-xs font-medium transition-all text-center p-0.5 leading-none rounded-full ${variantClassName}`}
        style={{ width: `${now}%` }}
      >
        {progressLabel}
      </div>
    </div>
  );
};
