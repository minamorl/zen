import { useForm } from "react-hook-form";
import { trpc } from "../utils/trpc";
import { useRouter } from "next/router";
import { useEffect } from "react";
import { useConsole } from "../context/consoleContext";
import Link from "next/link";

type Inputs = {
  email: string;
  password: string;
};
export default function SignInPage() {
  const {
    handleSubmit,
    register,
    formState: { errors },
  } = useForm<Inputs>();
  const { mutate: signIn, error } = trpc.signIn.useMutation();
  const router = useRouter();
  const [message, setMessage] = useConsole();
  useEffect(() => {
    setMessage("I don't know you yet. Please sign up!");
  }, []);

  const onSubmit = async (data: Inputs) => {
    try {
      signIn({ ...data });
      // try signin first and if it fails, try signUp
      router.push("/me");
    } catch (error) {
      console.error("Login failed", error);
    }
  };

  return (
    <div className="flex items-center justify-center ">
      <form
        onSubmit={handleSubmit(onSubmit)}
        className="p-6 white rounded shadow-md"
      >
        <div className="mb-4">
          <label
            htmlFor="email"
            className="block mb-2 text-sm font-bold text-gray-200"
          >
            Email
          </label>
          <input
            {...register("email", { required: "Email is required" })}
            type="email"
            className="w-full px-3 py-2 leading-tight text-gray-200 border rounded shadow appearance-none focus:outline-none focus:shadow-outline"
            placeholder="Email"
          />
        </div>

        <div className="mb-6">
          <label
            htmlFor="password"
            className="block mb-2 text-sm font-bold text-gray-200"
          >
            Password
          </label>
          <input
            {...register("password", { required: "Password is required" })}
            type="password"
            className="w-full px-3 py-2 mb-3 leading-tight text-gray-200 border rounded shadow appearance-none focus:outline-none focus:shadow-outline"
            placeholder="Password"
          />
        </div>

        <button
          type="submit"
          className="w-full px-4 py-2 font-bold text-white bg-blue-500 rounded hover:bg-blue-700 focus:outline-none focus:shadow-outline"
        >
          Sign In / Sign up
        </button>
        {error && <p className="mt-4 text-xs text-red-500">{error.message}</p>}
      </form>
    </div>
  );
}
