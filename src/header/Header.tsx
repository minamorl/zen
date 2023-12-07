import Link from "next/link";
import { ConsoleUI } from "@/console";
import { usePersona } from "../context/personaContext";
import { useConsole } from "../context/consoleContext";
import { useTheme } from "next-themes";
import { useEffect, useState } from "react";

const SignInButton = () => {
  const [persona] = usePersona();
  return (
    <button className="p-1 text-white bg-blue-600 text-[1rem]">
      {persona === "" ? <Link href={"/signup"}>Sign Up</Link> : <div></div>}
    </button>
  );
};

export const Header = () => {
  const [message] = useConsole();
  const theme = useTheme();
  const [headerBg, setHeaderBg] = useState("transparent");

  useEffect(() => {
    if (theme.theme === "dark") {
      setHeaderBg("black");
    } else {
      setHeaderBg("white");
    }
  }, [theme]);

  return (
    <header className={`px-4 pb-2 text-2xl sticky top-0 bg-${headerBg}`}>
      <div className="flex items-center">
        <h1 className="flex-none mr-8 w-6 font-extrabold display-block">
          <Link href="/">zen</Link>
        </h1>
        <div className="flex-1"></div>
        <SignInButton />
      </div>
      <div className="text-white ">
        <ConsoleUI message={message} />
      </div>
    </header>
  );
};
