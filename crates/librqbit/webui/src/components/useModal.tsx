import React, { useState, useEffect, useCallback, ReactNode } from "react";
import ReactDOM from "react-dom";

interface ModalProps {
  isOpen: boolean;
  closeModal: () => void;
  children: ReactNode;
}

interface UseModalOptions {
  fullScreen?: boolean;
  isOpen?: boolean;
}

const useModal = (
  options: UseModalOptions = {}
): [React.FC<ModalProps>, boolean, () => void, () => void] => {
  const [isOpen, setIsOpen] = useState(!!options.isOpen);

  const openModal = useCallback(() => {
    setIsOpen(true);
  }, []);

  const closeModal = useCallback(() => {
    setIsOpen(false);
  }, []);

  const handleEscape = useCallback(
    (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        closeModal();
      }
    },
    [closeModal]
  );

  const handleClickOutside = useCallback(
    (e: MouseEvent) => {
      const modalContent = document.querySelector(".modal-content");

      if (modalContent && !modalContent.contains(e.target as Node)) {
        closeModal();
      }
    },
    [closeModal]
  );

  useEffect(() => {
    const handleEscapeEvent = (e: KeyboardEvent): void => handleEscape(e);
    const handleClickOutsideEvent = (e: MouseEvent): void =>
      handleClickOutside(e);

    if (isOpen) {
      document.addEventListener("keydown", handleEscapeEvent);
      document.addEventListener("mousedown", handleClickOutsideEvent);
    }

    return () => {
      document.removeEventListener("keydown", handleEscapeEvent);
      document.removeEventListener("mousedown", handleClickOutsideEvent);
    };
  }, [isOpen, handleEscape, handleClickOutside]);

  const Modal: React.FC<ModalProps> = ({ children, isOpen, closeModal }) => {
    return isOpen
      ? ReactDOM.createPortal(
          <div className="fixed inset-0 z-50 overflow-hidden">
            <div className="fixed inset-0 transition-opacity">
              <div className="absolute inset-0 bg-gray-500 opacity-75"></div>
            </div>
            <div className="flex items-center justify-center min-h-screen p-4">
              <div
                className={`z-50 ${
                  options.fullScreen ? "w-full h-full" : "max-w-md"
                } modal-content animate-fade`}
              >
                <div className="relative bg-white w-full h-full mx-auto rounded shadow-lg">
                  <div className="p-6">{children}</div>
                  <button
                    className="absolute top-0 right-0 m-4 text-gray-600 hover:text-red-600"
                    onClick={closeModal}
                  >
                    &times;
                  </button>
                </div>
              </div>
            </div>
          </div>,
          document.body
        )
      : null;
  };

  return [Modal, isOpen, openModal, closeModal];
};

export default useModal;
