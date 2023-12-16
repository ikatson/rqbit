// Modal.tsx
import React, { type ReactNode } from "react";
import RestartModal from "@restart/ui/Modal";
import { BsX } from "react-icons/bs";

interface ModalProps {
  isOpen: boolean;
  onClose?: () => void;
  title: string;
  children: ReactNode;
  className?: string;
}

const ModalHeader: React.FC<{
  onClose?: () => void;
  title: string;
}> = ({ onClose, title }) => {
  return (
    <div className="flex p-3 justify-between items-center border-b dark:border-slate-600">
      <h2 className="text-xl font-semibold dark:slate-300">{title}</h2>
      {onClose && (
        <button
          className="text-gray-500 hover:text-gray-700"
          onClick={onClose}
          aria-label="Close modal"
        >
          <BsX className="w-5 h-5" />
        </button>
      )}
    </div>
  );
};

export const Modal: React.FC<ModalProps> = ({
  isOpen,
  onClose,
  title,
  children,
  className,
}) => {
  const renderBackdrop = () => {
    return (
      <div className="fixed inset-0 bg-black/30 z-[300] dark:bg-black/60 backdrop-blur"></div>
    );
  };
  return (
    <RestartModal
      show={isOpen}
      onHide={onClose}
      renderBackdrop={renderBackdrop}
      className="fixed z-[301] top-0 left-0 w-full h-full block overflow-x-hidden overflow-y-auto"
    >
      <div
        className={`bg-white shadow-lg my-8 mx-auto max-w-2xl rounded ${className} dark:bg-slate-800 dark:text-gray-50`}
      >
        <ModalHeader onClose={onClose} title={title} />
        {children}
      </div>
    </RestartModal>
  );
};
