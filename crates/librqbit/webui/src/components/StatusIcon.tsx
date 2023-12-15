import {
  MdCheckCircle,
  MdDownload,
  MdError,
  MdOutlineMotionPhotosPaused,
  MdOutlineUpload,
} from "react-icons/md";

type Props = {
  className?: string;
  finished: boolean;
  live: boolean;
  error: boolean;
};

export const StatusIcon = ({ className, finished, live, error }: Props) => {
  const isSeeding = finished && live;
  if (error) return <MdError className={className} color="red" />;
  if (isSeeding) return <MdOutlineUpload className={className} color="green" />;
  if (finished) return <MdCheckCircle className={className} color="green" />;
  if (live) return <MdDownload className={`text-blue-500 ${className}`} />;
  else return <MdOutlineMotionPhotosPaused className={className} />;
};
