import React, { useState, type ReactNode } from "react";
import { Modal } from "./Modal";
import { ModalBody } from "./ModalBody";
import { ModalFooter } from "./ModalFooter";
import { Button } from "../buttons/Button";
import { ErrorComponent } from "../ErrorComponent";
import { ErrorWithLabel } from "../../rqbit-web";

export interface ConfigTab {
  id: string;
  label: string;
  content: ReactNode;
}

export interface TabbedConfigModalProps {
  isOpen: boolean;
  onClose?: () => void;
  title: string;
  tabs: ConfigTab[];
  initialTab?: string;
  onSave: () => void | Promise<void>;
  onReset?: () => void;
  saveButtonText?: string;
  showResetButton?: boolean;
  isSaving?: boolean;
  error?: ErrorWithLabel | null;
}

export const TabbedConfigModal: React.FC<TabbedConfigModalProps> = ({
  isOpen,
  onClose,
  title,
  tabs,
  initialTab,
  onSave,
  onReset,
  saveButtonText = "OK",
  showResetButton = false,
  isSaving = false,
  error,
}) => {
  const [currentTab, setCurrentTab] = useState(initialTab || tabs[0]?.id);

  return (
    <Modal isOpen={isOpen} onClose={onClose} title={title} className="max-w-4xl">
      <ModalBody>
        <ErrorComponent error={error ?? null} />
        {tabs.length > 1 && (
          <div className="mb-4 flex border-b">
            {tabs.map((tab) => {
              const isActive = tab.id === currentTab;
              let classNames = "text-slate-300";
              if (isActive) {
                classNames =
                  "text-slate-800 border-b-2 border-blue-800 dark:border-blue-200 dark:text-white";
              }
              return (
                <button
                  key={tab.id}
                  className={`p-2 ${classNames}`}
                  onClick={() => setCurrentTab(tab.id)}
                >
                  {tab.label}
                </button>
              );
            })}
          </div>
        )}
        {tabs.map((tab) => (
          <div key={tab.id} className={tab.id === currentTab ? "" : "hidden"}>
            {tab.content}
          </div>
        ))}
      </ModalBody>
      <ModalFooter>
        {onClose && (
          <Button variant="cancel" onClick={onClose}>
            Cancel
          </Button>
        )}
        {showResetButton && onReset && (
          <Button variant="secondary" onClick={onReset}>
            Reset to defaults
          </Button>
        )}
        <Button variant="primary" onClick={onSave} disabled={isSaving}>
          {saveButtonText}
        </Button>
      </ModalFooter>
    </Modal>
  );
};
