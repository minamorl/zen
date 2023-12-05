import { useForm } from "react-hook-form";
import { trpc } from "../utils/trpc";
import { SubmitHandler } from "react-hook-form";
import { useRouter } from "next/router";

type Inputs = {
  email: string;
  password: string;
};
export default function SignInPage() {
  // react-hook-form
  //
  const { handleSubmit, register } = useForm<Inputs>();
  const { mutate, error } = trpc.signIn.useMutation();
  const router = useRouter();
  const onSubmit: SubmitHandler<Inputs> = async (inputs) => {
    const r = mutate({
      email: inputs.email,
      password: inputs.password,
    });
    router.push("/me");
  };
  return (
    <div>
      <form onSubmit={handleSubmit(onSubmit)}>
        <input
          {...register("email")}
          className="border border-gray-300 rounded-md"
          type="email"
          placeholder="email"
        />
        <input
          {...register("password")}
          className="border border-gray-300 rounded-md"
          type="password"
          placeholder="password"
        />
        <button type="submit">Submit</button>
        {error && <div className="text-red-500">{error.message}</div>}
      </form>
    </div>
  );
}
