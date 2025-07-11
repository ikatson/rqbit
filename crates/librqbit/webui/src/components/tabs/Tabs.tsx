import React, { ReactNode, useState } from "react";

export const Tab: React.FC<{
  children: ReactNode;
}> = ({ children }) => {
  return <>{children}</>;
};

export const Tabs: React.FC<{
  tabs: readonly string[];
  children: ReactNode;
}> = ({ tabs, children }) => {
  const [currentTab, setCurrentTab] = useState(tabs[0]);

  const tabChildren = React.Children.toArray(children);

  return (
    <div>
      <div className="mb-4 flex border-b">
        {tabs.map((t, i) => {
          const isActive = t === currentTab;
          let classNames = "text-slate-300 text-sm";
          if (isActive) {
            classNames =
              "text-slate-800 text-sm border-b-2 border-blue-500 dark:border-blue-200 dark:text-white";
          }
          return (
            <button
              key={i}
              className={`p-2 ${classNames}`}
              onClick={() => setCurrentTab(t)}
            >
              {t}
            </button>
          );
        })}
      </div>
      <div>
        {tabChildren.map((child, i) => {
          if (tabs[i] === currentTab) {
            return child;
          }
          return null;
        })}
      </div>
    </div>
  );
};
