import { useState } from "react";
import { Button, Modal, Form } from "react-bootstrap";

export const UrlPromptModal: React.FC<{
  show: boolean;
  setUrl: (_: string) => void;
  cancel: () => void;
}> = ({ show, setUrl, cancel }) => {
  let [inputValue, setInputValue] = useState("");
  return (
    <Modal show={show} onHide={cancel} size="lg">
      <Modal.Header closeButton>
        <Modal.Title>Add torrent</Modal.Title>
      </Modal.Header>
      <Modal.Body>
        <Form>
          <Form.Group className="mb-3" controlId="url">
            <Form.Label>Enter magnet or HTTP(S) URL to the .torrent</Form.Label>
            <Form.Control
              value={inputValue}
              placeholder="magnet:?xt=urn:btih:..."
              onChange={(u) => {
                setInputValue(u.target.value);
              }}
            />
          </Form.Group>
        </Form>
      </Modal.Body>
      <Modal.Footer>
        <Button
          variant="primary"
          onClick={() => {
            setUrl(inputValue);
            setInputValue("");
          }}
          disabled={inputValue.length == 0}
        >
          OK
        </Button>
        <Button variant="secondary" onClick={cancel}>
          Cancel
        </Button>
      </Modal.Footer>
    </Modal>
  );
};
