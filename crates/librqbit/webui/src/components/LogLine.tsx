import React from "react";
import { JSONLogLine, Span } from "../api-types";

const SpanFields: React.FC<{ span: Span }> = ({ span }) => {
  let fields = Object.entries(span).filter(([name, value]) => name != "name");
  if (fields.length == 0) {
    return null;
  }
  return (
    <>
      {"{"}
      {fields
        .map(([name, value]) => {
          return (
            <span key={name}>
              {name} = {value}
            </span>
          );
        })
        .reduce((prev, curr) => (
          <>
            {prev}, {curr}
          </>
        ))}
      {"}"}
    </>
  );
};

const LogSpan: React.FC<{ span: Span }> = ({ span }) => (
  <>
    <span className="font-bold">{span.name}</span>
    <SpanFields span={span} />
    <span className="font-bold">:</span>
  </>
);

const Fields: React.FC<{ fields: JSONLogLine["fields"] }> = ({ fields }) => (
  <span
    className={`m-1 ${
      fields.message.match(/error|fail/g)
        ? "text-red-500"
        : "text-slate-500 dark:text-slate-200"
    }`}
  >
    {fields.message}
    {Object.entries(fields)
      .filter(([key, value]) => key != "message")
      .map(([key, value]) => (
        <span className="m-1" key={key}>
          <span className="italic font-bold">{key}</span>={value}
        </span>
      ))}
  </span>
);

export const LogLine: React.FC<{ line: JSONLogLine }> = React.memo(
  ({ line }) => {
    const parsed = line;

    const classNameByLevel = (level: string) => {
      switch (level) {
        case "DEBUG":
          return "text-blue-500";
        case "INFO":
          return "text-green-500";
        case "WARN":
          return "text-amber-500";
        case "ERROR":
          return "text-red-500";
        default:
          return "text-slate-500";
      }
    };

    return (
      <p className="font-mono m-0 text-break text-[10px]">
        <span className="m-1 text-slate-500 dark:text-slate-400">
          {parsed.timestamp}
        </span>
        <span className={`m-1 ${classNameByLevel(parsed.level)}`}>
          {parsed.level}
        </span>

        <span className="m-1">
          {parsed.spans?.map((span, i) => <LogSpan key={i} span={span} />)}
        </span>
        <span className="m-1 text-slate-500 dark:text-slate-400">
          {parsed.target}
        </span>
        <Fields fields={parsed.fields} />
      </p>
    );
  }
);
