import { FileInput } from "./buttons/FileInput";
import { MagnetInput } from "./buttons/MagnetInput";

export const Header = ({ title }: { title: string }) => {
  const [name, version] = title.split("-");
  return (
    <header className="bg-slate-50 drop-shadow-lg flex flex-wrap justify-center lg:justify-between items-center mb-3">
      <div className="flex flex-nowrap m-2">
        <img src="/assets/logo.svg" className="w-10 h-10 p-1" alt="logo" />
        <h1 className="flex items-center">
          <div className="text-3xl">{name}</div>
          <div className="bg-blue-100 text-blue-800 text-xl font-semibold me-2 px-2.5 py-0.5 rounded ms-2">
            {version}
          </div>
        </h1>
      </div>
      <div className="flex flex-wrap gap-1 m-2">
        <MagnetInput />
        <FileInput />
      </div>
    </header>
  );
};
