import { useForm, SubmitHandler } from "react-hook-form";
import { trpc } from '../utils/trpc';
import { useEffect, useState } from 'react'


type Inputs = {
  email: string
  password: string
};
export default function SignInPage() {
  const {register, handleSubmit} = useForm<Inputs>()
  const { data , mutate } = trpc.signIn.useMutation()
  const onSubmit: SubmitHandler<Inputs> = (data) => {
    mutate({
      email: data.email,
      password: data.password
    })
  };


  const {data: personas} = trpc.getPersonas.useQuery()
  console.log(data)
    

  return (
    <div>
      <form onSubmit={handleSubmit(onSubmit)}>
        <input {...register("email")} />
        <input {...register("password")} />
        <input type="submit" />
      </form>
      <ul>
        { personas?.user && 'You are logged in as ' + personas.user.email}
        { personas?.personas && personas.personas.map(v => <li key={v.id}>{v.name}</li>) }
      </ul>
    </div>
  );

  
}
