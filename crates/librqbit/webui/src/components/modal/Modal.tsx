// Modal.tsx
import React, { useEffect, useState, type ReactNode } from "react";

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
  const [isModalOpen, setIsModalOpen] = useState(isOpen);

  useEffect(() => {
    setIsModalOpen(isOpen);
  }, [isOpen]);

  const closeModal = () => {
    setIsModalOpen(false);
    onClose();
  };

  const handleKeyDown = (event: React.KeyboardEvent<HTMLDivElement>) => {
    if (event.key === "Escape") {
      closeModal();
    }
  };

  return (
    <div
      className={`fixed top-0 left-0 w-full h-full flex items-center justify-center bg-opacity-50 ${
        isModalOpen ? "" : "hidden"
      }`}
      onClick={closeModal}
    >
      <div
        className="bg-white p-6 rounded shadow-lg"
        onClick={(e) => e.stopPropagation()}
        onKeyDown={handleKeyDown}
        role="dialog"
        aria-modal="true"
        aria-labelledby="modal-title"
        tabIndex={-1}
      >
        <div className="flex justify-between items-center border-b-2 pb-4">
          <h2 id="modal-title" className="text-2xl font-semibold">
            {title}
          </h2>
          <button
            className="text-gray-500 hover:text-gray-700 focus:outline-none"
            onClick={closeModal}
            aria-label="Close modal"
          >
            <svg
              className="h-6 w-6"
              fill="none"
              stroke="currentColor"
              viewBox="0 0 24 24"
              xmlns="http://www.w3.org/2000/svg"
            >
              <path
                stroke-linecap="round"
                stroke-linejoin="round"
                stroke-width="2"
                d="M6 18L18 6M6 6l12 12"
              ></path>
            </svg>
          </button>
        </div>
        <div className="mt-4">{children}</div>
      </div>
    </div>
  );
};
