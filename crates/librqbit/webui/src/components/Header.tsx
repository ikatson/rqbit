import { FileInput } from "./buttons/FileInput";
import { MagnetInput } from "./buttons/MagnetInput";

// @ts-ignore
import Logo from "../../assets/logo.svg?react";

export const Header = ({
  title,
  version,
  settingsSlot,
}: {
  title: string;
  version: string;
  settingsSlot?: React.ReactNode;
}) => {
  return (
    <header className="bg-surface-raised drop-shadow-lg flex flex-wrap justify-center lg:justify-between items-center">
      <div className="flex flex-nowrap items-center justify-between m-2">
        <Logo className="w-10 h-10 p-1" alt="logo" />
        <h1 className="flex items-center">
          <div className="text-3xl font-bold">{title}</div>
          <div className="bg-primary/10 text-primary text-xl font-semibold me-2 px-2.5 py-0.5 rounded ms-2">
            v{version}
          </div>
        </h1>
      </div>
      <div className="flex flex-wrap items-center gap-1 m-2">
        <MagnetInput className="grow justify-center" />
        <FileInput className="grow justify-center" />
        {settingsSlot && (
          <>
            <div className="hidden lg:block w-px h-6 bg-divider mx-2" />
            {settingsSlot}
          </>
        )}
      </div>
    </header>
  );
};
