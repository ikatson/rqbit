import { MagnetInput } from "./MagnetInput";
import { FileInput } from "./FileInput";

export const Buttons = () => {
  return (
    <div id="buttons-container" className="mt-3">
      <MagnetInput />
      <FileInput />
    </div>
  );
};
