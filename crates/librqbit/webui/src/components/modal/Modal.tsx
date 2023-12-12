// Modal.tsx
import React, { useEffect, useState, type ReactNode } from "react";
import RestartModal from "@restart/ui/Modal";
import { BsX } from "react-icons/bs";

interface ModalProps {
  isOpen: boolean;
  onClose: () => void;
  title: string;
  children: ReactNode;
}

export const Modal: React.FC<ModalProps> = ({
  isOpen,
  onClose,
  title,
  children,
}) => {
  const renderBackdrop = () => {
    return <div className="fixed inset-0 bg-black/30 z-[300]"></div>;
  };
  return (
    <RestartModal
      show={isOpen}
      onHide={onClose}
      renderBackdrop={renderBackdrop}
    >
      <div className="fixed z-[301] top-0 left-0 w-full">
        <div className="bg-white shadow-lg p-4 my-8 mx-auto max-w-2xl rounded">
          <div className="flex justify-between items-center border-b-2 pb-4">
            <h2 className="text-xl font-semibold">{title}</h2>
            <button
              className="text-gray-500 hover:text-gray-700"
              onClick={onClose}
              aria-label="Close modal"
            >
              <BsX className="w-5 h-5" />
            </button>
          </div>
          <div className="mt-4">{children}</div>
        </div>
      </div>
    </RestartModal>
  );
};
