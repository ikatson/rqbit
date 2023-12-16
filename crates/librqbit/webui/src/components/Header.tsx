import { FileInput } from "./buttons/FileInput";
import { MagnetInput } from "./buttons/MagnetInput";

// @ts-ignore
import Logo from "../../assets/logo.svg?react";

export const Header = ({ title }: { title: string }) => {
  const [name, version] = title.split("-");
  return (
    <header className="bg-slate-50 drop-shadow-lg flex flex-wrap justify-center lg:justify-between items-center dark:bg-slate-800 mb-3">
      <div className="flex flex-nowrap items-center justify-between m-2">
        <Logo className="w-10 h-10 p-1" alt="logo" />
        <h1 className="flex items-center dark:text-white">
          <div className="text-3xl">{name}</div>
          <div className="bg-blue-100 text-blue-800 text-xl font-semibold me-2 px-2.5 py-0.5 rounded ms-2 dark:bg-blue-900 dark:text-white">
            {version}
          </div>
        </h1>
      </div>
      <div className="flex flex-wrap gap-1 m-2">
        <MagnetInput className="flex-grow justify-center dark:text-white" />
        <FileInput className="flex-grow justify-center dark:text-white" />
      </div>
    </header>
  );
};
