import { BsX } from "react-icons/bs";
import { ErrorWithLabel } from "../rqbit-web";

const AlertDanger: React.FC<{
  title: string;
  children: React.ReactNode;
  onClose?: () => void;
}> = ({ title, children, onClose }) => {
  return (
    <div className="bg-red-200 p-3 rounded-md mb-3 dark:bg-red-800/60">
      <div className="flex justify-between mb-2">
        <h2 className="text-lg font-semibold">{title}</h2>
        {onClose && (
          <button onClick={onClose}>
            <BsX />
          </button>
        )}
      </div>
      {children}
    </div>
  );
};

export const ErrorComponent = (props: {
  error: ErrorWithLabel | null;
  remove?: () => void;
}) => {
  let { error, remove } = props;

  if (error == null) {
    return null;
  }

  return (
    <AlertDanger onClose={remove} title={error.text}>
      {error.details?.statusText && (
        <div className="pb-2 text-md">{error.details?.statusText}</div>
      )}
      <div className="whitespace-pre-wrap text-sm">{error.details?.text}</div>
    </AlertDanger>
  );
};
