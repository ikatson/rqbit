import { MouseEventHandler } from "react";

export const IconButton: React.FC<{
  className: string;
  onClick: () => void;
  disabled?: boolean;
  color?: string;
}> = ({ className, onClick, disabled, color }) => {
  const onClickStopPropagation: MouseEventHandler<HTMLAnchorElement> = (e) => {
    e.stopPropagation();
    if (disabled) {
      return;
    }
    onClick();
  };
  return (
    <a
      className={`bi ${className} p-1`}
      onClick={onClickStopPropagation}
      href="#"
    ></a>
  );
};
