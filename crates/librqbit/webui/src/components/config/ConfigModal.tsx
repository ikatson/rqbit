import React, { useContext, useEffect, useState } from "react";
import { TabbedConfigModal } from "../modal/TabbedConfigModal";
import { RateLimitsTab } from "./RateLimitsTab";
import { APIContext } from "../../context";
import { LimitsConfig, ErrorDetails } from "../../api-types";
import { ErrorWithLabel } from "../../rqbit-web";
import { Spinner } from "../Spinner";
import { Modal } from "../modal/Modal";
import { ModalBody } from "../modal/ModalBody";

export interface ConfigModalProps {
  isOpen: boolean;
  onClose: () => void;
}

export const ConfigModal: React.FC<ConfigModalProps> = ({
  isOpen,
  onClose,
}) => {
  const [limits, setLimits] = useState<LimitsConfig>({
    upload_bps: null,
    download_bps: null,
  });
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<ErrorWithLabel | null>(null);

  const API = useContext(APIContext);

  useEffect(() => {
    if (isOpen) {
      setLoading(true);
      setError(null);
      API.getLimits()
        .then((config) => {
          setLimits(config);
        })
        .catch((e: ErrorDetails) => {
          setError({ text: "Error loading limits", details: e });
        })
        .finally(() => setLoading(false));
    }
  }, [isOpen, API]);

  const handleSave = async () => {
    setSaving(true);
    setError(null);
    try {
      await API.setLimits(limits);
      onClose();
    } catch (e) {
      setError({ text: "Error saving limits", details: e as ErrorDetails });
    } finally {
      setSaving(false);
    }
  };

  if (loading && isOpen) {
    return (
      <Modal isOpen={isOpen} onClose={onClose} title="Configure">
        <ModalBody>
          <div className="flex justify-center p-4">
            <Spinner />
          </div>
        </ModalBody>
      </Modal>
    );
  }

  return (
    <TabbedConfigModal
      isOpen={isOpen}
      onClose={onClose}
      title="Configure"
      tabs={[
        {
          id: "limits",
          label: "Rate Limits",
          content: (
            <RateLimitsTab
              downloadBps={limits.download_bps}
              uploadBps={limits.upload_bps}
              onDownloadBpsChange={(v) =>
                setLimits((l) => ({ ...l, download_bps: v }))
              }
              onUploadBpsChange={(v) =>
                setLimits((l) => ({ ...l, upload_bps: v }))
              }
            />
          ),
        },
        {
          id: "other",
          label: "Other",
          content: (
            <div className="text-secondary py-2">
              <p>
                All other parameters (DHT, connections, persistence, etc.) can
                be configured via{" "}
                <code className="bg-surface-sunken px-1 rounded text-sm">
                  rqbit
                </code>{" "}
                CLI arguments when starting the server.
              </p>
              <p className="mt-2">
                Run{" "}
                <code className="bg-surface-sunken px-1 rounded text-sm">
                  rqbit --help
                </code>{" "}
                to see all available options.
              </p>
            </div>
          ),
        },
      ]}
      onSave={handleSave}
      isSaving={saving}
      error={error}
      showResetButton={false}
    />
  );
};
