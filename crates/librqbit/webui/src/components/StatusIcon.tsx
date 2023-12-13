import {
  MdCheck,
  MdCheckCircle,
  MdDownload,
  MdError,
  MdOutlineMotionPhotosPaused,
  MdOutlineUpload,
} from "react-icons/md";

type Props = {
  finished: boolean;
  isDownloading: boolean;
  error: boolean;
};

export const StatusIcon = ({ finished, isDownloading, error }: Props) => {
  const isSeeding = finished && isDownloading;
  if (error) return <MdError className="w-10 h-10" color="red" />;
  if (isSeeding) return <MdOutlineUpload className="w-10 h-10" color="green" />;
  if (finished) return <MdCheckCircle className="w-10 h-10" color="green" />;
  if (isDownloading) return <MdDownload className="w-10 h-10 text-blue-500" />;
  else return <MdOutlineMotionPhotosPaused className="w-10 h-10" />;
};
