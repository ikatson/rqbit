import { ChangeEventHandler, useId } from "react";

export const FormInput: React.FC<{
  value: string;
  label?: string;
  autoFocus?: boolean;
  name: string;
  inputType?: string;
  onKeyDown?: React.KeyboardEventHandler<HTMLInputElement>;
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
  onKeyDown,
  label,
  help,
  inputType,
  placeholder,
}) => {
  const id = useId();
  return (
    <div className="flex flex-col gap-2 mb-2">
      <label htmlFor={id}>{label}</label>
      <input
        autoFocus={autoFocus}
        type={inputType}
        className="block border border-divider rounded bg-transparent py-1.5 pl-2 focus:ring-0 focus:border-primary sm:leading-6"
        id={id}
        name={name}
        disabled={disabled}
        placeholder={placeholder}
        value={value}
        onKeyDown={onKeyDown}
        onChange={onChange}
      />
      {help && <div className="text-sm text-secondary">{help}</div>}
    </div>
  );
};
