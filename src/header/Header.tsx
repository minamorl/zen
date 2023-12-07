import Link from "next/link";
import { ConsoleUI } from "@/console";
import { usePersona } from "../context/personaContext";
import { useConsole } from "../context/consoleContext";
import { useTheme } from "next-themes";
const SignInButton = () => {
  const [persona] = usePersona();
  return (
    <div className="p-1 text-white bg-blue-600 text-md">
      {persona === "" ? <Link href={"/signup"}>Sign Up</Link> : <div></div>}
    </div>
  );
};

export const Header = () => {
  const [message] = useConsole();
  const theme = useTheme();

  const headerStyle = {
    backgroundColor: theme.theme === "dark" ? "#000000" : "#ffffff",
  };

  return (
    <header className="px-4 pb-2 text-2xl sticky top-0" style={headerStyle}>
      <div className="flex">
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
