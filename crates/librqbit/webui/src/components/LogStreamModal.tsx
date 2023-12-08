import { useContext } from "react";
import { Button, Modal } from "react-bootstrap";
import { APIContext } from "../context";
import { ErrorComponent } from "./ErrorComponent";
import { LogStream } from "./LogStream";

interface Props {
  show: boolean;
  onClose: () => void;
}

export const LogStreamModal: React.FC<Props> = ({ show, onClose }) => {
  const api = useContext(APIContext);
  const apiBase = api.getHttpBaseUrl();

  return (
    <Modal size="xl" show={show} onHide={onClose}>
      <Modal.Header closeButton>
        <Modal.Title>rqbit server logs</Modal.Title>
      </Modal.Header>
      <Modal.Body>
        {apiBase ? (
          <LogStream httpApiBase={apiBase} />
        ) : (
          <ErrorComponent
            error={{ text: "HTTP API not available to stream logs" }}
          ></ErrorComponent>
        )}
      </Modal.Body>
      <Modal.Footer>
        <Button variant="primary" onClick={onClose}>
          Close
        </Button>
      </Modal.Footer>
    </Modal>
  );
};
