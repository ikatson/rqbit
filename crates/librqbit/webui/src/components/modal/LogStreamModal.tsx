import { useContext } from "react";
import { APIContext } from "../../context";
import { ErrorComponent } from "../ErrorComponent";
import { LogStream } from "../LogStream";
import { Modal } from "./Modal";
import { ModalFooter } from "./ModalFooter";
import { ModalBody } from "./ModalBody";
import { Button } from "../buttons/Button";

interface Props {
  show: boolean;
  onClose: () => void;
}

export const LogStreamModal: React.FC<Props> = ({ show, onClose }) => {
  const api = useContext(APIContext);
  let logsUrl = api.getStreamLogsUrl();

  return (
    <Modal
      isOpen={show}
      onClose={onClose}
      title="rqbit server logs"
      className="max-w-7xl"
    >
      <ModalBody>
        {logsUrl ? (
          <LogStream url={logsUrl} />
        ) : (
          <ErrorComponent
            error={{ text: "HTTP API not available to stream logs" }}
          ></ErrorComponent>
        )}
      </ModalBody>
      <ModalFooter>
        <Button variant="primary" onClick={onClose}>
          Close
        </Button>
      </ModalFooter>
    </Modal>
  );
};
