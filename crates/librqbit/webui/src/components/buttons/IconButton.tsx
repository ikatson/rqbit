import { MouseEventHandler } from "react";

export const IconButton: React.FC<{
  onClick: () => void;
  disabled?: boolean;
  className?: string;
  color?: string;
  children: any;
}> = (props) => {
  const { onClick, disabled, color, children, className, ...otherProps } =
    props;
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
      className={`block p-1 text-blue-500 flex items-center justify-center ${colorClassName} ${className}`}
      onClick={onClickStopPropagation}
      href="#"
      {...otherProps}
    >
      {children}
    </a>
  );
};
