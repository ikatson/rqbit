// Modal.tsx

import React, { useState, useEffect } from "react";

interface ModalProps {
  isOpen: boolean;
  onClose: () => void;
  children: React.ReactNode;
}

const Modal: React.FC<ModalProps> = ({ isOpen, onClose, children }) => {
  const [modalOpen, setModalOpen] = useState(isOpen);

  useEffect(() => {
    setModalOpen(isOpen);
  }, [isOpen]);

  const closeModal = () => {
    setModalOpen(false);
    onClose();
  };

  return (
    <div
      className={`fixed inset-0 overflow-y-auto ${
        modalOpen ? "opacity-100" : "opacity-0 pointer-events-none"
      } transition-opacity duration-300 ease-in-out`}
    >
      <div className="flex items-center justify-center min-h-screen pt-4 px-4 pb-20 text-center">
        <div
          className="fixed inset-0 transition-opacity"
          aria-hidden="true"
          onClick={closeModal}
        >
          <div className="absolute inset-0 bg-black opacity-50"></div>
        </div>

        <span
          className="hidden sm:inline-block sm:align-middle sm:h-screen"
          aria-hidden="true"
        >
          &#8203;
        </span>

        <div
          className={`inline-block align-bottom bg-white rounded-lg text-left overflow-hidden shadow-xl transform transition-all sm:my-8 sm:align-middle sm:w-full sm:max-w-lg ${
            modalOpen ? "opacity-100 translate-y-0" : "opacity-0 translate-y-4"
          }`}
        >
          {children}
        </div>
      </div>
    </div>
  );
};

export default Modal;
