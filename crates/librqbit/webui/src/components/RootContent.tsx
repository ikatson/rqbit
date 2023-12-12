import { useContext } from "react";
import { TorrentId, ErrorDetails as ApiErrorDetails } from "../api-types";
import { AppContext } from "../context";
import { TorrentsList } from "./TorrentsList";
import { ErrorComponent } from "./ErrorComponent";

export const RootContent = (props: {
  closeableError: ApiErrorDetails | null;
  otherError: ApiErrorDetails | null;
  torrents: Array<TorrentId> | null;
  torrentsLoading: boolean;
}) => {
  let ctx = useContext(AppContext);
  return (
    <div className="container mx-auto">
      <ErrorComponent
        error={props.closeableError}
        remove={() => ctx.setCloseableError(null)}
      />
      <ErrorComponent error={props.otherError} />
      <TorrentsList torrents={props.torrents} loading={props.torrentsLoading} />
    </div>
  );
};
