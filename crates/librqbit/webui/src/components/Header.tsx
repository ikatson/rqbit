import Logo from "/assets/logo.svg";
import { Buttons } from "./buttons/Buttons";

export const Header = ({ title }: { title: string }) => {
  const [name, version] = title.split("-");
  return (
    <header className="p-2 bg-slate-50 drop-shadow-lg flex items-center justify-between mb-5">
      <div className="flex gap-2 items-center">
        <img
          src={Logo}
          className="bg-white rounded-xl p-1 w-10 h-10"
          alt="logo"
        />
        <h1 className="flex items-center text-3xl font-extrabold">
          {name}
          <span className="bg-blue-100 text-blue-800 text-2xl font-semibold me-2 px-2.5 py-0.5 rounded ms-2">
            {version}
          </span>
        </h1>
      </div>
      <Buttons />
    </header>
  );
};
