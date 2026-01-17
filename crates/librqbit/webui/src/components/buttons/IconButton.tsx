import { MouseEventHandler } from "react";

export const IconButton: React.FC<{
  onClick: () => void;
  disabled?: boolean;
  className?: string;
  color?: string;
  children: any;
  href?: string;
  title?: string;
}> = (props) => {
  const {
    onClick,
    disabled,
    color,
    children,
    className,
    href,
    title,
    ...otherProps
  } = props;
  const onClickStopPropagation: MouseEventHandler<HTMLAnchorElement> = (e) => {
    e.stopPropagation();
    e.preventDefault();
    if (disabled) {
      return;
    }
    onClick();
  };
  const colorClassName = color ? `text-${color}` : "";
  return (
    <a
      className={`p-1 text-blue-500 flex items-center justify-center cursor-pointer ${colorClassName} ${className}`}
      onClick={onClickStopPropagation}
      href={href ?? "#"}
      title={title}
      {...otherProps}
    >
      {children}
    </a>
  );
};
