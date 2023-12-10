import { MagnetInput } from "./MagnetInput";
import { FileInput } from "./FileInput";

export const Buttons = () => {
  return (
    <div id="buttons-container" className="flex gap-2">
      <MagnetInput />
      <FileInput />
    </div>
  );
};
