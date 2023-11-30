import { useForm, SubmitHandler } from "react-hook-form";
import { trpc } from '../utils/trpc';
import { useEffect, useState } from 'react'


type Inputs = {
  email: string
  password: string
};
export default function SignInPage() {
  const [accessToken, setAccesssToken] = useState('')
  const {register, handleSubmit} = useForm<Inputs>()
  const { data: signin, mutate } = trpc.signIn.useMutation()
  const onSubmit: SubmitHandler<Inputs> = (data) => {
    mutate({
      email: data.email,
      password: data.password
    })
  };
  useEffect(() => signin && setAccesssToken(signin.user.session.access_token), [signin])


  const {data} = trpc.me.useQuery()
  console.log(data)
    

  return (
    <form onSubmit={handleSubmit(onSubmit)}>
      <input {...register("email")} />
      <input {...register("password")} />
      <input type="submit" />
    </form>
  );

  
}
