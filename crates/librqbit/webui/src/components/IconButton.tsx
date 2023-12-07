import { MouseEventHandler } from "react";

export const IconButton: React.FC<{
  onClick: () => void;
  disabled?: boolean;
  color?: string;
  children: any;
}> = ({ onClick, disabled, color, children }) => {
  const onClickStopPropagation: MouseEventHandler<HTMLAnchorElement> = (e) => {
    e.stopPropagation();
    if (disabled) {
      return;
    }
    onClick();
  };
  const colorClassName = color ? `text-${color}` : "";
  return (
    <a
      className={`p-1 ${colorClassName}`}
      onClick={onClickStopPropagation}
      href="#"
    >
      {children}
    </a>
  );
};
