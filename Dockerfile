# Using Node.js 20
FROM node:20

# Setting the working directory
WORKDIR /app

RUN npm install -g pnpm
# Copying package.json and package-lock.json
#COPY package.json package-lock.json ./
#COPY prisma ./prisma/

# Installing dependencies
#RUN npm install

# Copying the rest of the application's code
COPY . .

# Exposing port 3000
EXPOSE 3000

# Starting the development server
CMD ["pnpm", "run", "dev"]
