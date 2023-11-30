import { useEffect, useState } from 'react';
import { trpc } from '../utils/trpc';
import { SubmitHandler, useForm } from 'react-hook-form';

type Inputs = {
  raw_text: string
};


export default function IndexPage() {
  const {data, isLoading, mutate} = trpc.createPost.useMutation()
  const [key, setKey] = useState('invalid')
  const {handleSubmit, register} = useForm<Inputs>()
  const {data: posts, refetch, isRefetching} = trpc.getPosts.useQuery()
  const { data: me} = trpc.getPersonas.useQuery()

  const onSubmit: SubmitHandler<Inputs> = async (inputs) => {
    mutate({
      raw_text: inputs.raw_text,
      persona_id: me?.personas?.[0].id ?? '' ,
    })
    setKey(inputs.raw_text)
    setTimeout(() => refetch(), 1000)
  }


  if (!posts) {
    return <div>Loading...</div>;
  }
  return (
    <div>
      <div>
        Your persona id is {me && Array.isArray(me.personas) && me.personas[0].id}
      </div>
      <form onSubmit={handleSubmit(onSubmit)}>
        <input
          {...register("raw_text")}
        />
        <input type="submit" value="submit" />
      </form>
      <div>
        <ul>
          { posts.map(v => <li key={v.id} className="shadow p-4 m-4 rounded">
            <div>{v.raw_text}</div>
            <div>{v.created_at}</div>
          </li>) }
          { isRefetching && <li key={key}>{key}</li>}
        </ul>
      </div>
    </div>
  );
}
