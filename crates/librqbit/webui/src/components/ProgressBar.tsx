const variantClassNames = {
  warn: "bg-amber-500 text-white",
  info: "bg-blue-500 text-white",
  success: "bg-green-700 text-white",
  error: "bg-red-500 text-white",
};

export const ProgressBar: React.FC<{
  now: number;
  label?: string | null;
  variant?: "warn" | "info" | "success" | "error";
  classNames?: string;
}> = ({ now, variant, label, classNames }) => {
  const progressLabel = label ?? `${now.toFixed(2)}%`;

  const variantClassName =
    variantClassNames[variant ?? "info"] ?? variantClassNames["info"];

  return (
    <div
      className={`w-full bg-gray-200 rounded-full mb-1 dark:bg-gray-500 ${classNames}`}
    >
      <div
        className={`text-xs font-medium transition-all text-center leading-none py-0.5 px-2 rounded-full ${variantClassName} ${
          now < 1 && "bg-transparent"
        }`}
        style={{ width: `${now}%` }}
      >
        {progressLabel}
      </div>
    </div>
  );
};
