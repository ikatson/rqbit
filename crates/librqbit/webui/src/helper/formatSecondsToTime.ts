export function formatSecondsToTime(seconds: number): string {
  const days = Math.floor(seconds / 86400);
  const hours = Math.floor((seconds % 86400) / 3600);
  const minutes = Math.floor((seconds % 3600) / 60);
  const remainingSeconds = seconds % 60;

  const formatUnit = (value: number, unit: string) =>
    value > 0 ? `${value}${unit}` : "";

  if (days > 0) {
    return `${formatUnit(days, "d")} ${formatUnit(hours, "h")} ${formatUnit(minutes, "m")}`.trim();
  } else if (hours > 0) {
    return `${formatUnit(hours, "h")} ${formatUnit(minutes, "m")}`.trim();
  } else if (minutes > 0) {
    return `${formatUnit(minutes, "m")} ${formatUnit(remainingSeconds, "s")}`.trim();
  } else {
    return `${formatUnit(remainingSeconds, "s")}`.trim();
  }
}
