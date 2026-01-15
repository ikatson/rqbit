const variantClassNames = {
  warn: "bg-warning-bg text-white",
  info: "bg-primary-bg text-white",
  success: "bg-success-bg text-white",
  error: "bg-error-bg text-white",
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
    <div className={`w-full bg-divider rounded-full mb-1 ${classNames}`}>
      <div
        className={`text-sm font-medium transition-all text-center leading-none py-0.5 px-2 rounded-full ${variantClassName} ${
          now < 1 && "bg-transparent"
        }`}
        style={{ width: `${now}%` }}
      >
        {progressLabel}
      </div>
    </div>
  );
};
