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
    <header className="bg-slate-50 drop-shadow-lg flex flex-wrap justify-center lg:justify-between items-center dark:bg-slate-800">
      <div className="flex flex-nowrap items-center justify-between m-2">
        <Logo className="w-10 h-10 p-1" alt="logo" />
        <h1 className="flex items-center dark:text-white">
          <div className="text-3xl">{title}</div>
          <div className="bg-blue-100 text-blue-800 text-xl font-semibold me-2 px-2.5 py-0.5 rounded ms-2 dark:bg-blue-900 dark:text-white">
            v{version}
          </div>
        </h1>
      </div>
      <div className="flex flex-wrap items-center gap-1 m-2">
        <MagnetInput className="grow justify-center dark:text-white" />
        <FileInput className="grow justify-center dark:text-white" />
        {settingsSlot && (
          <>
            <div className="hidden lg:block w-px h-6 bg-gray-300 dark:bg-slate-600 mx-2" />
            {settingsSlot}
          </>
        )}
      </div>
    </header>
  );
};
