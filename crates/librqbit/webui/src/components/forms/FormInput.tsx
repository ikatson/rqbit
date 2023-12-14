import { ChangeEventHandler } from "react";

export const FormInput: React.FC<{
  value: string;
  label?: string;
  autoFocus?: boolean;
  name: string;
  inputType?: string;
  placeholder?: string;
  help?: string;
  disabled?: boolean;
  onChange?: ChangeEventHandler<HTMLInputElement>;
}> = ({
  autoFocus,
  value,
  name,
  disabled,
  onChange,
  label,
  help,
  inputType,
  placeholder,
}) => {
  return (
    <div className="flex flex-col gap-2 text-sm mb-6">
      <label htmlFor={name}>{label}</label>
      <input
        autoFocus={autoFocus}
        type={inputType}
        className="block border rounded bg-transparent py-1.5 pl-2 text-gray-800 focus:ring-0 sm:text-sm sm:leading-6"
        id={name}
        name={name}
        disabled={disabled}
        placeholder={placeholder}
        value={value}
        onChange={onChange}
      />
      {help && <div className="text-xs text-slate-500 mb-3">{help}</div>}
    </div>
  );
};
