import React from "react";

export interface TabButtonProps {
  id: string;
  label: string;
  active: boolean;
  onClick: () => void;
}

export const TabButton: React.FC<TabButtonProps> = ({
  label,
  active,
  onClick,
}) => (
  <button
    onClick={onClick}
    className={`px-3 py-1.5 font-medium transition-colors cursor-pointer ${
      active
        ? "border-b-2 border-primary text-primary -mb-px"
        : "text-tertiary hover:text-secondary"
    }`}
  >
    {label}
  </button>
);

export interface TabListProps {
  children: React.ReactNode;
  className?: string;
}

export const TabList: React.FC<TabListProps> = ({ children, className }) => (
  <div className={`flex border-b border-divider ${className ?? ""}`}>
    {children}
  </div>
);
