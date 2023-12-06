import React, { useEffect, useState } from "react";
import { useConsole } from "../context/consoleContext";
type ConsoleProps = {
  message: string;
};

export const ConsoleUI: React.FC<ConsoleProps> = () => {
  const [message] = useConsole();
  const [mouseOver, setMouseOver] = useState(false);

  return (
    <div
      className="text-sm font-mono cursor-default"
      onMouseOver={() => setMouseOver(true)}
      onMouseLeave={() => setMouseOver(false)}
    >
      <div className="flex flex-row">
        <div className="block w-6">⚡</div>
        <div className="block">
          {mouseOver ? "Hi! I'm Navi. What are you looking for?" : message}
        </div>
        <div className="block animate-ping">_</div>
      </div>
    </div>
  );
};
