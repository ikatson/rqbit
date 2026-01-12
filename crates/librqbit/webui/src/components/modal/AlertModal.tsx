import { ErrorWithLabel } from "../../rqbit-web";
import { useErrorStore } from "../../stores/errorStore";
import { Button } from "../buttons/Button";
import { Modal } from "./Modal";
import { ModalBody } from "./ModalBody";
import { ModalFooter } from "./ModalFooter";

export const AlertModal: React.FC<{}> = () => {
  let alert = useErrorStore((store) => store.alert);
  let removeAlert = useErrorStore((store) => store.removeAlert);

  if (alert) {
    return (
      <Modal isOpen={true} onClose={removeAlert} title={alert.text}>
        <ModalBody>
          {alert.details?.statusText && (
            <div className="pb-2 text-md">{alert.details?.statusText}</div>
          )}
          <div className="whitespace-pre-wrap">{alert.details?.text}</div>
        </ModalBody>
        <ModalFooter>
          <Button variant="cancel" onClick={removeAlert}>
            Close
          </Button>
        </ModalFooter>
      </Modal>
    );
  } else {
    return <></>;
  }
};
