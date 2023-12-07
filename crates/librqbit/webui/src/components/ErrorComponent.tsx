import { Alert } from "react-bootstrap";
import { Error } from "../rqbit-web";

export const ErrorComponent = (props: {
  error: Error | null;
  remove?: () => void;
}) => {
  let { error, remove } = props;

  if (error == null) {
    return null;
  }

  return (
    <Alert variant="danger" onClose={remove} dismissible={remove != null}>
      <Alert.Heading>{error.text}</Alert.Heading>
      {error.details?.statusText && (
        <p>
          <strong>{error.details?.statusText}</strong>
        </p>
      )}
      <pre>{error.details?.text}</pre>
    </Alert>
  );
};
