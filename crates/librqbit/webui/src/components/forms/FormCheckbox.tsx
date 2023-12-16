import { ChangeEventHandler } from "react";

export const FormCheckbox: React.FC<{
  checked: boolean;
  label: string;
  name: string;
  help?: string;
  disabled?: boolean;
  inputType?: "checkbox" | "switch";
  onChange?: ChangeEventHandler<HTMLInputElement>;
}> = ({ checked, name, disabled, onChange, label, help, inputType }) => {
  return (
    <div className="flex gap-3 items-start">
      <div className="flex">
        <input
          type={inputType || "checkbox"}
          className="block mt-1"
          id={name}
          name={name}
          disabled={disabled}
          checked={checked}
          onChange={onChange}
        />
      </div>
      <div className="text-sm flex flex-col gap-1">
        <label htmlFor={name}>{label}</label>
        {help && (
          <div className="text-xs text-slate-500 dark:text-slate-300 mb-3">
            {help}
          </div>
        )}
      </div>
    </div>
  );
};
